import type { ReactNode, RefObject } from "react";

import { Detail } from "@/components/ui/detail-list";
import { Notice } from "@/components/ui/notice";
import { executionTargetName } from "@/domain/execution-target";
import { DeploymentSummaryStep } from "@/features/deployments/deployment-summary-step";
import type { PredictedMemory } from "@/features/models/deploy/memory/model-deploy-memory";
import { MemoryCapacityWarning } from "@/features/models/deploy/memory/model-deploy-memory-strip";
import {
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "@/features/models/runtime/engine-parameter-values";
import type { ChangedEngineParameter } from "@/features/models/runtime/local-deployment-configuration";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import { formatBytes, formatCompactCount } from "@/lib/format";

import type { LocalDeploymentMemoryPreview } from "./use-model-deploy-memory-preview";

/**
 * The deploy dialog's Deploy step: the deployment name and route, notices about the site and the
 * launch's memory, and a summary of the target, profile, context, each changed parameter and the
 * other customized advanced settings.
 */
export function ModelDeploySummaryStep({
  changedParameters,
  customizations,
  disabled,
  exposureFields,
  filledContextPerRequest,
  inputs,
  launchValuesLoadingVisible,
  memoryPreview,
  name,
  nameError,
  nameRef,
  onNameBlur,
  onNameChange,
  poolBelowDeviceMinimum,
  predictedMemory,
  profileName,
  siteNotice,
  target,
}: ModelDeploySummaryStepProps) {
  return (
    <DeploymentSummaryStep
      disabled={disabled}
      name={name}
      nameError={nameError}
      nameInputId="model-deployment-name"
      nameRef={nameRef}
      notices={
        <>
          {siteNotice}
          {launchValuesLoadingVisible ? (
            <p className="text-xs text-muted-foreground" role="status">
              Loading launch values…
            </p>
          ) : null}
          {predictedMemory ? <MemoryCapacityWarning memory={predictedMemory} /> : null}
          {poolBelowDeviceMinimum && memoryPreview ? (
            <Notice tone="warning">{poolBelowDeviceMinimumMessage(memoryPreview)}</Notice>
          ) : null}
        </>
      }
      onNameBlur={onNameBlur}
      onNameChange={onNameChange}
      nameFields={exposureFields}
    >
      <Detail label="Target" value={target ? executionTargetName(target) : "None"} />
      <Detail label="Profile" value={profileName} />
      <Detail label="Context" value={contextSummary(inputs, filledContextPerRequest)} />
      {changedParameters.map(({ parameter, value }) => (
        <Detail
          key={parameter.parameterId}
          label={parameter.displayName}
          value={capitalized(value)}
        />
      ))}
      {customizations.length ? (
        <Detail label="Advanced" value={`Customized: ${customizations.join(" · ")}`} />
      ) : null}
    </DeploymentSummaryStep>
  );
}

export interface ModelDeploySummaryStepProps {
  /** The engine parameters outside the context whose values differ from the defaults. */
  changedParameters: readonly ChangedEngineParameter[];
  /** The other advanced settings that differ from the defaults, in a few words each. */
  customizations: readonly string[];
  disabled: boolean;
  /** The route fields shown under the name. */
  exposureFields: ReactNode;
  /** The context per request resolution fills in when the launch leaves it empty. */
  filledContextPerRequest?: number;
  /** The launch's engine parameter inputs. */
  inputs: ReadonlyMap<string, string>;
  launchValuesLoadingVisible: boolean;
  memoryPreview?: LocalDeploymentMemoryPreview;
  name: string;
  nameError?: string;
  nameRef: RefObject<HTMLInputElement | null>;
  onNameBlur: () => void;
  onNameChange: (name: string) => void;
  /** Whether the target's system memory is below the profile's per-device minimum. */
  poolBelowDeviceMinimum: boolean;
  predictedMemory?: PredictedMemory;
  profileName: string;
  /** A notice about the site's execution targets, when none can deploy. */
  siteNotice?: ReactNode;
  target?: ConfiguredExecutionTarget;
}

/**
 * The launch's context in a few words, such as "128k total · 64k per request". An empty context
 * per request reads as the value resolution fills in.
 */
function contextSummary(
  inputs: ReadonlyMap<string, string>,
  filledPerRequest: number | undefined,
): string {
  const tokens = (input: string) =>
    /^\d+$/.test(input) ? formatCompactCount(Number(input)) : input;
  const total = inputs.get(TOTAL_CONTEXT_PARAMETER_ID) ?? "";
  const perRequest =
    inputs.get(CONTEXT_PER_REQUEST_PARAMETER_ID) ||
    (filledPerRequest === undefined ? "" : String(filledPerRequest));
  return [total && `${tokens(total)} total`, perRequest && `${tokens(perRequest)} per request`]
    .filter(Boolean)
    .join(" · ");
}

function capitalized(text: string): string {
  return `${text.charAt(0).toUpperCase()}${text.slice(1)}`;
}

/** The warning for system memory below the profile's minimum, with sizes when known. */
function poolBelowDeviceMinimumMessage({
  memoryPoolTotalBytes,
  minimumMemoryBytesPerDevice,
}: LocalDeploymentMemoryPreview): string {
  const comparison =
    memoryPoolTotalBytes !== undefined && minimumMemoryBytesPerDevice !== undefined
      ? `This target's system memory (${formatBytes(memoryPoolTotalBytes)}) is below this profile's ${formatBytes(minimumMemoryBytesPerDevice)} minimum.`
      : "This target's system memory is below this profile's minimum.";

  return `${comparison} The model may fail to load or slow down the machine.`;
}
