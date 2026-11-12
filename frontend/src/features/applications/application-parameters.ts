import { create } from "@bufbuild/protobuf";

import {
  type ParameterView,
  type ParameterViewValue,
  textValueError,
} from "@/components/parameters/parameter-view";
import type { ResolvedApplicationDeploymentSnapshot } from "@/gen/dotcore/application/v1/deployment_pb";
import {
  type ParameterDefinition,
  type ParameterValue,
  ParameterValueSchema,
  ParameterVisibility,
} from "@/gen/dotcore/catalog/v1/parameter_pb";

/** One application parameter as the deploy dialog shows it, with its catalog default. */
export interface ApplicationParameterField {
  advanced: boolean;
  defaultValue: ParameterViewValue | undefined;
  view: ParameterView;
}

/** The application's parameters in declaration order; malformed declarations are skipped. */
export function applicationParameterFields(
  parameters: readonly ParameterDefinition[] | undefined,
): ApplicationParameterField[] {
  return (parameters ?? []).flatMap((definition): ApplicationParameterField[] => {
    const advanced = definition.visibility === ParameterVisibility.ADVANCED;
    const base = { id: definition.parameterId, label: definition.displayName };
    const type = definition.parameterType;
    switch (type.case) {
      case "boolean":
        return [
          {
            advanced,
            defaultValue: type.value.defaultValue,
            view: { ...base, description: definition.description, type: { kind: "boolean" } },
          },
        ];
      case "unsignedInteger": {
        const { defaultValue, maximum, minimum, unit } = type.value;
        return [
          {
            advanced,
            defaultValue,
            view: {
              ...base,
              description: definition.description,
              type: { kind: "unsignedInteger", maximum, minimum, unit },
            },
          },
        ];
      }
      case "stringEnum":
        return [
          {
            advanced,
            defaultValue: type.value.defaultValue,
            view: {
              ...base,
              description: definition.description,
              type: { kind: "stringEnum", values: type.value.values },
            },
          },
        ];
      case "text": {
        const { defaultValue, maximumLength, minimumLength } = type.value;
        return [
          {
            advanced,
            defaultValue,
            view: {
              ...base,
              description: definition.description,
              type: { kind: "text", maximumLength, minimumLength },
            },
          },
        ];
      }
      case "secret":
        return [
          {
            advanced,
            defaultValue: undefined,
            view: {
              ...base,
              description: definition.description,
              type: { kind: "secret", minimumLength: type.value.minimumLength },
            },
          },
        ];
      default:
        return [];
    }
  });
}

/** Form inputs holding each parameter's catalog default. */
export function defaultParameterInputs(
  fields: readonly ApplicationParameterField[],
): Map<string, string> {
  return new Map(
    fields.map((field) => [
      field.view.id,
      field.defaultValue === undefined ? "" : String(field.defaultValue),
    ]),
  );
}

/** Why an input is not a valid value for its parameter, or `undefined` when it is. */
export function parameterInputError(view: ParameterView, input: string): string | undefined {
  if (view.type.kind === "secret" && input === "") return undefined;
  if (parameterInputValue(view, input) !== undefined) return undefined;
  if (view.type.kind === "text" || view.type.kind === "secret") {
    const forbidden = textValueError(view.label, input);
    if (forbidden) return forbidden;
    const { minimumLength } = view.type;
    return view.type.kind === "secret"
      ? `${view.label} must be at least ${minimumLength} characters.`
      : `${view.label} must be ${minimumLength} to ${view.type.maximumLength} characters.`;
  }
  if (view.type.kind === "unsignedInteger") {
    const { maximum, minimum, unit } = view.type;
    return `${view.label} must be an integer from ${minimum} to ${maximum}${unit ? ` ${unit}` : ""}.`;
  }
  return `Select a ${view.label.toLocaleLowerCase()}.`;
}

/** The typed value an input holds, or `undefined` when it is not valid for the parameter. */
export function parameterInputValue(
  view: ParameterView,
  input: string,
): ParameterViewValue | undefined {
  switch (view.type.kind) {
    case "boolean":
      return input === "true" ? true : input === "false" ? false : undefined;
    case "stringEnum":
      return view.type.values.includes(input) ? input : undefined;
    case "text":
    case "secret": {
      const length = [...input].length;
      const { minimumLength } = view.type;
      const maximumLength = view.type.kind === "text" ? view.type.maximumLength : Infinity;
      const valid =
        length >= minimumLength && length <= maximumLength && !textValueError("", input);
      return valid ? input : undefined;
    }
    case "unsignedInteger": {
      if (!/^\d+$/.test(input.trim())) return undefined;
      const value = BigInt(input.trim());
      return value >= view.type.minimum && value <= view.type.maximum ? value : undefined;
    }
  }
}

/** The parameters whose valid input differs from its catalog default, in declaration order. */
export function changedParameterValues(
  fields: readonly ApplicationParameterField[],
  inputs: ReadonlyMap<string, string>,
): { field: ApplicationParameterField; value: ParameterViewValue }[] {
  return fields.flatMap((field) => {
    const value = parameterInputValue(field.view, inputs.get(field.view.id) ?? "");
    return value === undefined || value === field.defaultValue ? [] : [{ field, value }];
  });
}

/** The creation request's overrides: only parameters the operator changed from their default. */
export function parameterOverrides(
  fields: readonly ApplicationParameterField[],
  inputs: ReadonlyMap<string, string>,
): ParameterValue[] {
  return changedParameterValues(fields, inputs).map(({ field, value }) =>
    create(ParameterValueSchema, {
      parameterId: field.view.id,
      value:
        typeof value === "boolean"
          ? { case: "boolean", value }
          : typeof value === "bigint"
            ? { case: "unsignedInteger", value }
            : { case: "string", value },
    }),
  );
}

/** Each parameter's catalog default, keyed by parameter ID. */
export function parameterDefaults(
  fields: readonly ApplicationParameterField[],
): Map<string, ParameterViewValue | undefined> {
  return new Map(fields.map((field) => [field.view.id, field.defaultValue]));
}

/** The values a deployment retained, keyed by parameter ID. */
export function retainedParameterValues(
  snapshot: ResolvedApplicationDeploymentSnapshot | undefined,
): Map<string, ParameterViewValue> {
  const values = new Map<string, ParameterViewValue>();
  for (const value of snapshot?.parameterValues ?? []) {
    if (value.value.case !== undefined) values.set(value.parameterId, value.value.value);
  }
  return values;
}
