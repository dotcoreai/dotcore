import type { ReactNode, RefObject } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DeploymentForm,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import type { EngineParameterDefinition } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { Model, ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import type { FieldErrors } from "@/lib/form-validation";
import { formatCompactCount } from "@/lib/format";

import { ContextSizeField } from "./context-size-field";
import type { ContextSizeRange } from "./context-size-input";
import {
  engineParameterHelp,
  filledContextPerRequest,
  maximumContext,
  runtimeParameterGroups,
} from "./engine-parameter-values";

interface ModelDeployContextStepProps {
  /** The largest total context that fits the selected target, shown beside the total context. */
  contextHint?: string;
  disabled: boolean;
  engineParameterRefs: RefObject<Map<string, HTMLElement | null>>;
  exposedParameters: readonly EngineParameterDefinition[];
  fieldErrors: FieldErrors;
  inputs: ReadonlyMap<string, string>;
  model: Model;
  onParameterBlur: (parameter: EngineParameterDefinition) => void;
  /** Receives each change, and whether it is settled, as a slider or preset is, or typed. */
  onParameterChange: (
    parameter: EngineParameterDefinition,
    input: string,
    settled?: boolean,
  ) => void;
  profile?: ModelProfile;
  /**
   * Where the total context comes from: its label, and a reset back to the default once the
   * operator chose it. Absent while the engine default applies.
   */
  totalSource?: { label: string; onReset?: () => void };
}

/**
 * The Context step of the model deploy dialog: the total context the deployment holds and the
 * context one request may use, which follows the total until set. Each field shows only when the
 * selected profile exposes its engine parameter.
 */
export function ModelDeployContextStep({
  contextHint,
  disabled,
  engineParameterRefs,
  exposedParameters,
  fieldErrors,
  inputs,
  model,
  onParameterBlur,
  onParameterChange,
  profile,
  totalSource,
}: ModelDeployContextStepProps) {
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );
  const { contextPerRequest, totalContext } = runtimeParameterGroups(
    exposedParameters,
    configuration,
  ).context;
  const maximum = maximumContext(model, configuration);
  // Total context and context per request follow each other, so their sliders share one scale.
  const sliderRange = contextSliderRange([totalContext, contextPerRequest]);
  const contextField = (
    parameter: EngineParameterDefinition,
    field: { emptyValue?: number; help?: ReactNode; hint?: string; source?: ReactNode } = {},
  ) => (
    <ContextSizeField
      disabled={disabled}
      error={fieldErrors.get(parameter.parameterId)}
      input={inputs.get(parameter.parameterId) ?? ""}
      inputRefs={engineParameterRefs}
      onBlur={onParameterBlur}
      onChange={onParameterChange}
      parameter={parameter}
      sliderRange={sliderRange}
      {...field}
    />
  );

  return (
    <DeploymentForm className="gap-5">
      <DeploymentStepHeading>Context</DeploymentStepHeading>
      {totalContext || contextPerRequest ? null : (
        <p className="text-xs text-muted-foreground" role="status">
          This profile sets its context itself.
        </p>
      )}
      {totalContext
        ? contextField(totalContext, {
            help: engineParameterHelp(totalContext),
            hint: contextHint,
            source: totalSource ? (
              <>
                <Badge variant="outline">{totalSource.label}</Badge>
                {totalSource.onReset ? (
                  <Button
                    disabled={disabled}
                    onClick={totalSource.onReset}
                    size="xs"
                    type="button"
                    variant="link"
                  >
                    Use largest fit
                  </Button>
                ) : null}
              </>
            ) : undefined,
          })
        : null}
      {contextPerRequest
        ? contextField(contextPerRequest, {
            emptyValue: filledContextPerRequest(model, configuration, inputs),
            help: inputs.get(contextPerRequest.parameterId) ? (
              <Button
                className="h-auto p-0 text-xs"
                disabled={disabled}
                onClick={() => onParameterChange(contextPerRequest, "", true)}
                type="button"
                variant="link"
              >
                Follow the total
              </Button>
            ) : (
              "Follows the total, up to the model maximum."
            ),
            hint:
              maximum === undefined
                ? undefined
                : `Up to ${formatCompactCount(maximum)} (model maximum)`,
          })
        : null}
    </DeploymentForm>
  );
}

/** The range spanning every given context size parameter's schema range, if any is present. */
function contextSliderRange(
  parameters: readonly (EngineParameterDefinition | undefined)[],
): ContextSizeRange | undefined {
  const ranges = parameters.flatMap((parameter) =>
    parameter?.parameterType.case === "unsignedInteger"
      ? [
          {
            maximum: Number(parameter.parameterType.value.maximum),
            minimum: Number(parameter.parameterType.value.minimum),
          },
        ]
      : [],
  );
  if (!ranges.length) return undefined;

  return {
    maximum: Math.max(...ranges.map((range) => range.maximum)),
    minimum: Math.min(...ranges.map((range) => range.minimum)),
  };
}
