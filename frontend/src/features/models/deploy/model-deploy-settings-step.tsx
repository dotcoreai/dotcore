import type { ReactNode, RefObject } from "react";

import { Button } from "@/components/ui/button";
import {
  DeploymentForm,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import { contextSliderEnds, requestCount } from "@/features/models/runtime/context-slider-ends";
import {
  maximumContext,
  PARALLEL_REQUESTS_PARAMETER_ID,
  runtimeParameterGroups,
} from "@/features/models/runtime/engine-parameter-values";
import { EngineParametersForm } from "@/features/models/runtime/engine-parameters";
import type { MemoryParameterSizes } from "@/features/models/runtime/memory-parameter-sizes";
import { SizeField } from "@/features/models/runtime/size-field";
import type { SizeLargestFit, SizeRange } from "@/features/models/runtime/size-input";
import type { Model, ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ParameterDefinition, ParameterGroup } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { FieldErrors } from "@/lib/form-validation";
import { formatCompactCount } from "@/lib/format";

interface ModelDeploySettingsStepProps {
  /** The largest values that fit the selected target, offered as each context field's last chip. */
  contextFits: ContextValues<SizeLargestFit>;
  /** The largest values that fit the selected target, in each context field's help. */
  contextHints: ContextValues<string>;
  disabled: boolean;
  engineParameterRefs: RefObject<Map<string, HTMLElement | null>>;
  /** The context parameters the profile exposes. */
  exposedParameters: readonly ParameterDefinition[];
  fieldErrors: FieldErrors;
  /** The context per request the backend fills while the input is empty, from the latest estimate. */
  filledContextPerRequest?: number;
  inputs: ReadonlyMap<string, string>;
  /** The latest estimate's memory parameter sizes, for the memory parameters' sliders. */
  memoryParameterSizes?: MemoryParameterSizes;
  model: Model;
  /** Makes the total context follow the context per request and parallel requests again. */
  onFollowTotal: () => void;
  onParameterBlur: (parameter: ParameterDefinition) => void;
  /** Receives each change, and whether it is settled, as a slider or preset is, or typed. */
  onParameterChange: (parameter: ParameterDefinition, input: string, settled?: boolean) => void;
  /** The basic engine parameters outside the context, which follow the context fields. */
  parameters: {
    /** The engine's parameter groups the configuration keeps. */
    groups: readonly ParameterGroup[];
    onBlur: (parameter: ParameterDefinition) => void;
    onChange: (parameter: ParameterDefinition, input: string) => void;
    /** The errors of the latest blocked navigation, which open the groups holding them. */
    revealedErrors: FieldErrors;
    shown: readonly ParameterDefinition[];
  };
  profile?: ModelProfile;
  /** Whether the total context follows the context per request and parallel requests. */
  totalFollows: boolean;
}

/** One value for each context size field. */
export interface ContextValues<T> {
  perRequest?: T;
  total?: T;
}

/**
 * The Settings step of the model deploy dialog: the context one request may use, the requests
 * served at once, and the total context they share, which follows the other two until set, then
 * the profile's other basic engine parameters. Each field shows only when the selected profile
 * exposes its engine parameter.
 */
export function ModelDeploySettingsStep({
  contextFits,
  contextHints,
  disabled,
  engineParameterRefs,
  exposedParameters,
  fieldErrors,
  filledContextPerRequest,
  inputs,
  memoryParameterSizes,
  model,
  onFollowTotal,
  onParameterBlur,
  onParameterChange,
  parameters,
  profile,
  totalFollows,
}: ModelDeploySettingsStepProps) {
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );
  const { contextPerRequest, parallelRequests, totalContext } =
    runtimeParameterGroups(exposedParameters).context;
  const maximum = maximumContext(model, configuration);
  const ends = contextSliderEnds({
    contextPerRequest,
    maximumContext: maximum,
    parallelRequests: requestCount(inputs.get(PARALLEL_REQUESTS_PARAMETER_ID)),
    totalContext,
  });
  // Total context and context per request follow each other, so their sliders share one scale.
  const sliderRange = ends && {
    maximum: Math.max(ends.perRequest, ends.total),
    minimum: ends.minimum,
  };
  const contextField = (
    parameter: ParameterDefinition,
    end: number | undefined,
    field: {
      actions?: ReactNode;
      emptyValue?: number;
      facts?: readonly string[];
      help?: string;
      largestFit?: SizeLargestFit;
    } = {},
  ) => (
    <SizeField
      disabled={disabled}
      error={fieldErrors.get(parameter.parameterId)}
      input={inputs.get(parameter.parameterId) ?? ""}
      inputRefs={engineParameterRefs}
      onBlur={onParameterBlur}
      onChange={onParameterChange}
      parameter={parameter}
      range={fieldRange(parameter, end)}
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
      {contextPerRequest
        ? contextField(contextPerRequest, ends?.perRequest, {
            emptyValue: filledContextPerRequest,
            facts: [
              ...(contextHints.perRequest ? [contextHints.perRequest] : []),
              ...(maximum === undefined
                ? []
                : [`Up to ${formatCompactCount(maximum)} (model maximum)`]),
            ],
            help: contextPerRequest.description,
            largestFit: contextFits.perRequest,
          })
        : null}
      {parallelRequests ? (
        <EngineParametersForm
          errors={fieldErrors}
          groups={[]}
          inputRefs={engineParameterRefs}
          inputs={inputs}
          onBlur={onParameterBlur}
          onChange={(parameter, input) => onParameterChange(parameter, input)}
          parameters={[parallelRequests]}
          revealedErrors={parameters.revealedErrors}
        />
      ) : null}
      {totalContext
        ? contextField(totalContext, ends?.total, {
            actions: totalFollows ? undefined : (
              <Button
                className="h-auto p-0 text-xs"
                disabled={disabled}
                onClick={onFollowTotal}
                type="button"
                variant="link"
              >
                Follow per request × parallel
              </Button>
            ),
            facts: contextHints.total ? [contextHints.total] : undefined,
            help: totalContext.description,
            largestFit: contextFits.total,
          })
        : null}
      {parameters.shown.length ? (
        <EngineParametersForm
          errors={fieldErrors}
          groups={parameters.groups}
          inputRefs={engineParameterRefs}
          inputs={inputs}
          memoryParameterSizes={memoryParameterSizes}
          onBlur={parameters.onBlur}
          onChange={parameters.onChange}
          parameters={parameters.shown}
          revealedErrors={parameters.revealedErrors}
        />
      ) : null}
    </DeploymentForm>
  );
}

/** The values a context field's slider spans: from its own minimum up to its derived end. */
function fieldRange(
  parameter: ParameterDefinition,
  end: number | undefined,
): SizeRange | undefined {
  if (end === undefined || parameter.parameterType.case !== "unsignedInteger") return undefined;

  return { maximum: end, minimum: Number(parameter.parameterType.value.minimum) };
}
