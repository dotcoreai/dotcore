import type { RefObject } from "react";

import { ParametersDetails, ParametersForm } from "@/components/parameters/parameters";
import { DetailList } from "@/components/ui/detail-list";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import {
  parameterGroupView,
  parameterValueMap,
  parameterView,
} from "@/domain/parameter-definitions";
import type { ParameterView } from "@/domain/parameter-view";
import type { EngineParameterSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import {
  type ParameterDefinition,
  type ParameterGroup,
  type ParameterValues,
  ParameterVisibility,
} from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";
import type { FieldErrors } from "@/lib/form-validation";
import { isSizeUnit } from "@/lib/format";

import { effectiveVisibility } from "./engine-parameter-values";
import type { MemoryParameterSizes } from "./memory-parameter-sizes";
import { SizeField } from "./size-field";

interface EngineParametersFormProps {
  errors: FieldErrors;
  /** The engine's parameter groups, which lay out the parameters that name one. */
  groups: readonly ParameterGroup[];
  inputs: ReadonlyMap<string, string>;
  inputRefs: RefObject<Map<string, HTMLElement | null>>;
  /**
   * The latest estimate's memory parameter sizes: a memory parameter with a largest fit gets a
   * slider ending there, and an empty one shows the value sizing picks.
   */
  memoryParameterSizes?: MemoryParameterSizes;
  onBlur: (parameter: ParameterDefinition) => void;
  onChange: (parameter: ParameterDefinition, input: string, settled?: boolean) => void;
  parameters: readonly ParameterDefinition[];
}

/**
 * Edits one deploy step's engine parameters, laid out by the engine's parameter groups. A memory
 * parameter takes the shared size input, which reads typed sizes in any byte unit.
 */
export function EngineParametersForm({
  errors,
  groups,
  inputs,
  inputRefs,
  memoryParameterSizes,
  onBlur,
  onChange,
  parameters,
}: EngineParametersFormProps) {
  const definitions = new Map(parameters.map((parameter) => [parameter.parameterId, parameter]));
  const memoryField = (view: ParameterView) => {
    const parameter = definitions.get(view.id);
    if (
      !parameter ||
      view.type.kind !== "unsignedInteger" ||
      !view.type.memoryPool ||
      !isSizeUnit(view.type.unit)
    )
      return undefined;

    const fit = memoryParameterSizes?.fits.get(view.id);
    return (
      <SizeField
        emptyValue={memoryParameterSizes?.sized.get(view.id)}
        error={errors.get(view.id)}
        help={view.description || undefined}
        input={inputs.get(view.id) ?? ""}
        inputRefs={inputRefs}
        key={view.id}
        largestFit={fit === undefined ? undefined : { value: fit }}
        onBlur={onBlur}
        onChange={onChange}
        parameter={parameter}
        summary={view.summary}
        range={fit === undefined ? undefined : { maximum: fit, minimum: Number(view.type.minimum) }}
      />
    );
  };

  return (
    <ParametersForm
      errors={errors}
      groups={groups.map(parameterGroupView)}
      idPrefix="engine-parameter"
      inputRefs={inputRefs}
      inputs={inputs}
      onBlur={(parameterId) => {
        const parameter = definitions.get(parameterId);
        if (parameter) onBlur(parameter);
      }}
      onChange={(parameterId, input, settled) => {
        const parameter = definitions.get(parameterId);
        if (parameter) onChange(parameter, input, settled);
      }}
      parameters={engineParameterViews(parameters)}
      renderField={memoryField}
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
