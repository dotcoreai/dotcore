import type { ParameterDefinition, ParameterValue } from "@/gen/dotcore/catalog/v1/parameter_pb";

import type { ParameterView, ParameterViewValue } from "./parameter-view";

/**
 * A catalog parameter definition as the console renders and validates it, for applications and
 * engines alike. Undefined for a definition without a type.
 */
export function parameterView(definition: ParameterDefinition): ParameterView | undefined {
  const base = {
    description: definition.description,
    id: definition.parameterId,
    label: definition.displayName,
  };
  const type = definition.parameterType;
  switch (type.case) {
    case "boolean":
      return { ...base, type: { kind: "boolean" } };
    case "unsignedInteger": {
      const { maximum, minimum, unit } = type.value;
      return { ...base, type: { kind: "unsignedInteger", maximum, minimum, unit } };
    }
    case "stringEnum":
      return { ...base, type: { kind: "stringEnum", values: type.value.values } };
    case "text": {
      const { maximumLength, minimumLength } = type.value;
      return { ...base, type: { kind: "text", maximumLength, minimumLength } };
    }
    case "secret": {
      const { maximumLength, minimumLength } = type.value;
      return { ...base, type: { kind: "secret", maximumLength, minimumLength } };
    }
    default:
      return undefined;
  }
}

/** The definition's catalog default, or undefined when it declares none. A secret never has one. */
export function parameterDefaultValue(
  definition: ParameterDefinition,
): ParameterViewValue | undefined {
  const type = definition.parameterType;
  switch (type.case) {
    case "boolean":
    case "unsignedInteger":
    case "stringEnum":
    case "text":
      return type.value.defaultValue;
    default:
      return undefined;
  }
}

/** Parameter values keyed by parameter ID; a value without a case is skipped. */
export function parameterValueMap(
  values: readonly ParameterValue[] | undefined,
): Map<string, ParameterViewValue> {
  const map = new Map<string, ParameterViewValue>();
  for (const value of values ?? []) {
    if (value.value.case !== undefined) map.set(value.parameterId, value.value.value);
  }
  return map;
}

/** A typed value as the value case a parameter value carries. */
export function parameterValueCase(value: ParameterViewValue): ParameterValue["value"] {
  switch (typeof value) {
    case "boolean":
      return { case: "boolean", value };
    case "bigint":
      return { case: "unsignedInteger", value };
    case "string":
      return { case: "string", value };
  }
}
