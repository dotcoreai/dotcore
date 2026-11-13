import type { RefObject } from "react";

import type { ParameterView, ParameterViewValue } from "@/components/parameters/parameter-view";
import { ParametersDetails, ParametersForm } from "@/components/parameters/parameters";
import { DetailList } from "@/components/ui/detail-list";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import type { EngineParameterSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import {
  type ParameterDefinition,
  type ParameterValues,
  ParameterVisibility,
} from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";

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

  return (
    <ParametersDetails
      parameters={engineParameterViews(schema.parameters)}
      values={parameterValueMap(values)}
    />
  );
}

/**
 * A model deployment's Parameters section: every engine parameter it uses that is basic or advanced
 * for its profile, in schema order, read from the snapshot alone.
 */
export function DeploymentParametersSection({
  snapshot,
}: {
  snapshot: ResolvedLocalModelDeploymentSnapshot;
}) {
  const { engineParameterSchema: schema, engineParameterValues: values } = snapshot;
  if (!schema || !values) return null;

  const shown = schema.parameters.filter((parameter) => {
    const visibility =
      snapshot.profileEngineParameterVisibilityOverrides.find(
        (override) => override.parameterId === parameter.parameterId,
      )?.visibility ?? parameter.visibility;
    return visibility !== ParameterVisibility.HIDDEN;
  });
  const retained = parameterValueMap(values);
  const views = engineParameterViews(shown).filter((view) => retained.has(view.id));
  if (!views.length) return null;

  return (
    <section aria-labelledby="deployment-parameters-heading">
      <InspectorSectionHeader headingId="deployment-parameters-heading" title="Parameters" />
      <DetailList>
        <ParametersDetails parameters={views} values={retained} />
      </DetailList>
    </section>
  );
}

function parameterValueMap(values: ParameterValues | undefined): Map<string, ParameterViewValue> {
  const map = new Map<string, ParameterViewValue>();
  for (const value of values?.values ?? []) {
    if (value.value.case !== undefined) map.set(value.parameterId, value.value.value);
  }
  return map;
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
      case "stringEnum":
        return [
          { ...base, type: { kind: "stringEnum", values: parameter.parameterType.value.values } },
        ];
      case "text": {
        const { maximumLength, minimumLength } = parameter.parameterType.value;
        return [{ ...base, type: { kind: "text", maximumLength, minimumLength } }];
      }
      case "secret":
        return [
          {
            ...base,
            type: { kind: "secret", minimumLength: parameter.parameterType.value.minimumLength },
          },
        ];
      default:
        return [];
    }
  });
}
