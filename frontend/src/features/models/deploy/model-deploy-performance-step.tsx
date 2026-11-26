import type { ReactNode, RefObject } from "react";

import { Notice } from "@/components/ui/notice";
import {
  DeploymentForm,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import { EngineParametersForm } from "@/features/models/runtime/engine-parameters";
import type { MemoryParameterSizes } from "@/features/models/runtime/memory-parameter-sizes";
import type { ParameterDefinition, ParameterGroup } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { FieldErrors } from "@/lib/form-validation";

interface ModelDeployPerformanceStepProps {
  engineParameterRefs: RefObject<Map<string, HTMLElement | null>>;
  /** The advanced engine parameters outside the context. */
  exposedParameters: readonly ParameterDefinition[];
  fieldErrors: FieldErrors;
  inputs: ReadonlyMap<string, string>;
  /** Warns that host KV offload shares the accelerator's memory pool on this target. */
  hostKvOffloadSharesMemory: boolean;
  /** The latest estimate's memory parameter sizes, for the memory parameters' sliders. */
  memoryParameterSizes?: MemoryParameterSizes;
  onParameterBlur: (parameter: ParameterDefinition) => void;
  onParameterChange: (parameter: ParameterDefinition, input: string) => void;
  /** The advanced model option fields, after the parameters. */
  options: ReactNode;
  /** The engine's parameter groups the configuration keeps. */
  parameterGroups: readonly ParameterGroup[];
  /** The errors of the latest blocked navigation; a new value opens the groups it flags. */
  revealedErrors: FieldErrors;
}

/**
 * The Performance & memory step of the model deploy dialog, shown in advanced mode: the profile's
 * advanced engine parameters laid out by the engine's parameter groups, then the advanced model
 * options. A collapsed group opens when a blocked navigation flags one of its fields, so the field
 * can take focus.
 */
export function ModelDeployPerformanceStep({
  engineParameterRefs,
  exposedParameters,
  fieldErrors,
  hostKvOffloadSharesMemory,
  inputs,
  memoryParameterSizes,
  onParameterBlur,
  onParameterChange,
  options,
  parameterGroups,
  revealedErrors,
}: ModelDeployPerformanceStepProps) {
  return (
    <DeploymentForm className="gap-4">
      <DeploymentStepHeading>Performance &amp; memory</DeploymentStepHeading>
      {exposedParameters.length ? (
        <EngineParametersForm
          errors={fieldErrors}
          groups={parameterGroups}
          inputRefs={engineParameterRefs}
          inputs={inputs}
          memoryParameterSizes={memoryParameterSizes}
          onBlur={onParameterBlur}
          onChange={onParameterChange}
          parameters={exposedParameters}
          revealedErrors={revealedErrors}
        />
      ) : null}
      {hostKvOffloadSharesMemory ? (
        <Notice tone="warning">
          KV cache offload adds no capacity on this target. Its GPU already uses shared system
          memory, so offloaded cache takes memory from the same pool.
        </Notice>
      ) : null}
      {options}
    </DeploymentForm>
  );
}
