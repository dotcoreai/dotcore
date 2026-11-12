import type { ReactNode, RefObject } from "react";

import { Button } from "@/components/ui/button";
import {
  DeploymentForm,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import { ContextSizeField } from "@/features/models/runtime/context-size-field";
import type {
  ContextSizeLargestFit,
  ContextSizeRange,
} from "@/features/models/runtime/context-size-input";
import {
  maximumContext,
  runtimeParameterGroups,
} from "@/features/models/runtime/engine-parameter-values";
import { EngineParametersForm } from "@/features/models/runtime/engine-parameters";
import type { Model, ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ParameterDefinition } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { FieldErrors } from "@/lib/form-validation";
import { formatCompactCount } from "@/lib/format";

interface ModelDeploySettingsStepProps {
  /** The largest total context that fits the selected target, shown in the total context's help. */
  contextHint?: string;
  disabled: boolean;
  engineParameterRefs: RefObject<Map<string, HTMLElement | null>>;
  exposedParameters: readonly ParameterDefinition[];
  fieldErrors: FieldErrors;
  /** The context per request the backend fills while the input is empty, from the latest estimate. */
  filledContextPerRequest?: number;
  inputs: ReadonlyMap<string, string>;
  /** The largest total context that fits the selected target, offered as the total's last chip. */
  largestFit?: ContextSizeLargestFit;
  model: Model;
  onParameterBlur: (parameter: ParameterDefinition) => void;
  /** Receives each change, and whether it is settled, as a slider or preset is, or typed. */
  onParameterChange: (parameter: ParameterDefinition, input: string, settled?: boolean) => void;
  /** The basic engine parameters outside the context, which follow the context fields. */
  parameters: {
    onBlur: (parameter: ParameterDefinition) => void;
    onChange: (parameter: ParameterDefinition, input: string) => void;
    shown: readonly ParameterDefinition[];
  };
  profile?: ModelProfile;
}

/**
 * The Settings step of the model deploy dialog: the total context the deployment holds and the
 * context one request may use, which follows the total until set, then the profile's other basic
 * engine parameters. Each field shows only when the selected profile exposes its engine parameter.
 */
export function ModelDeploySettingsStep({
  contextHint,
  disabled,
  engineParameterRefs,
  exposedParameters,
  fieldErrors,
  filledContextPerRequest,
  inputs,
  largestFit,
  model,
  onParameterBlur,
  onParameterChange,
  parameters,
  profile,
}: ModelDeploySettingsStepProps) {
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
    parameter: ParameterDefinition,
    field: {
      actions?: ReactNode;
      emptyValue?: number;
      facts?: readonly string[];
      help?: string;
      largestFit?: ContextSizeLargestFit;
    } = {},
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
      <DeploymentStepHeading>Settings</DeploymentStepHeading>
      {totalContext || contextPerRequest ? null : (
        <p className="text-xs text-muted-foreground" role="status">
          This profile sets its context itself.
        </p>
      )}
      {totalContext
        ? contextField(totalContext, {
            facts: contextHint ? [contextHint] : undefined,
            help: totalContext.description,
            largestFit,
          })
        : null}
      {contextPerRequest
        ? contextField(contextPerRequest, {
            actions: inputs.get(contextPerRequest.parameterId) ? (
              <Button
                className="h-auto p-0 text-xs"
                disabled={disabled}
                onClick={() => onParameterChange(contextPerRequest, "", true)}
                type="button"
                variant="link"
              >
                Follow the total
              </Button>
            ) : undefined,
            emptyValue: filledContextPerRequest,
            facts: [
              ...(maximum === undefined
                ? []
                : [`Up to ${formatCompactCount(maximum)} (model maximum)`]),
              ...(inputs.get(contextPerRequest.parameterId)
                ? []
                : ["Left empty, it follows the total context."]),
            ],
            help: contextPerRequest.description,
          })
        : null}
      {parameters.shown.length ? (
        <EngineParametersForm
          errors={fieldErrors}
          inputRefs={engineParameterRefs}
          inputs={inputs}
          onBlur={parameters.onBlur}
          onChange={parameters.onChange}
          parameters={parameters.shown}
        />
      ) : null}
    </DeploymentForm>
  );
}

/** The range spanning every given context size parameter's schema range, if any is present. */
function contextSliderRange(
  parameters: readonly (ParameterDefinition | undefined)[],
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
