import type { RefObject } from "react";

import type { ParameterView, ParameterViewValue } from "@/components/parameters/parameter-view";
import { ParametersDetails, ParametersForm } from "@/components/parameters/parameters";
import type { EngineParameterSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { ParameterDefinition, ParameterValues } from "@/gen/dotcore/catalog/v1/parameter_pb";

interface EngineParametersFormProps {
  errors: ReadonlyMap<string, string>;
  inputs: ReadonlyMap<string, string>;
  inputRefs: RefObject<Map<string, HTMLElement | null>>;
  onBlur: (parameter: ParameterDefinition) => void;
  onChange: (parameter: ParameterDefinition, input: string) => void;
  parameters: readonly ParameterDefinition[];
}

export function EngineParametersForm({
  errors,
  inputs,
  inputRefs,
  onBlur,
  onChange,
  parameters,
}: EngineParametersFormProps) {
  const definitions = new Map(parameters.map((parameter) => [parameter.parameterId, parameter]));

  return (
    <ParametersForm
      errors={errors}
      idPrefix="engine-parameter"
      inputRefs={inputRefs}
      inputs={inputs}
      onBlur={(parameterId) => {
        const parameter = definitions.get(parameterId);
        if (parameter) onBlur(parameter);
      }}
      onChange={(parameterId, input) => {
        const parameter = definitions.get(parameterId);
        if (parameter) onChange(parameter, input);
      }}
      parameters={engineParameterViews(parameters)}
    />
  );
}

export function EngineParametersDetails({
  schema,
  values,
}: {
  schema: EngineParameterSchema | undefined;
  values: ParameterValues | undefined;
}) {
  if (!schema || !values) return null;

  const retained = new Map<string, ParameterViewValue>();
  for (const value of values.values) {
    if (value.value.case !== undefined) retained.set(value.parameterId, value.value.value);
  }
  return (
    <ParametersDetails parameters={engineParameterViews(schema.parameters)} values={retained} />
  );
}

function engineParameterViews(parameters: readonly ParameterDefinition[]): ParameterView[] {
  return parameters.flatMap((parameter): ParameterView[] => {
    const base = {
      description: parameter.description,
      id: parameter.parameterId,
      label: parameter.displayName,
    };
    switch (parameter.parameterType.case) {
      case "boolean":
        return [{ ...base, type: { kind: "boolean" } }];
      case "unsignedInteger": {
        const { maximum, minimum, unit } = parameter.parameterType.value;
        return [{ ...base, type: { kind: "unsignedInteger", maximum, minimum, unit } }];
      }
      default:
        return [];
    }
  });
}
