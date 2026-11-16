import { create } from "@bufbuild/protobuf";

import {
  parameterDefaultValue,
  parameterValueCase,
  parameterValueMap,
  parameterView,
} from "@/domain/parameter-definitions";
import {
  parameterInputValue,
  type ParameterView,
  type ParameterViewValue,
} from "@/domain/parameter-view";
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
    const view = parameterView(definition);
    if (!view) return [];
    return [
      {
        advanced: definition.visibility === ParameterVisibility.ADVANCED,
        defaultValue: parameterDefaultValue(definition),
        view,
      },
    ];
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
    create(ParameterValueSchema, { parameterId: field.view.id, value: parameterValueCase(value) }),
  );
}

/** The values a deployment retained, keyed by parameter ID. */
export function retainedParameterValues(
  snapshot: ResolvedApplicationDeploymentSnapshot | undefined,
): Map<string, ParameterViewValue> {
  return parameterValueMap(snapshot?.parameterValues);
}
