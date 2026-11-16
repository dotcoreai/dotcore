import type { RefObject } from "react";

import { ParametersDetails, ParametersForm } from "@/components/parameters/parameters";
import { DetailList } from "@/components/ui/detail-list";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { parameterValueMap, parameterView } from "@/domain/parameter-definitions";
import type { ParameterView } from "@/domain/parameter-view";
import type { EngineParameterSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import {
  type ParameterDefinition,
  type ParameterValues,
  ParameterVisibility,
} from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";

import { effectiveVisibility } from "./engine-parameter-values";

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
      values={parameterValueMap(values.values)}
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

  const overrides = snapshot.profileEngineParameterVisibilityOverrides;
  const shown = schema.parameters.filter(
    (parameter) => effectiveVisibility(parameter, overrides) !== ParameterVisibility.HIDDEN,
  );
  const retained = parameterValueMap(values.values);
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

function engineParameterViews(parameters: readonly ParameterDefinition[]): ParameterView[] {
  return parameters.flatMap((parameter) => parameterView(parameter) ?? []);
}
