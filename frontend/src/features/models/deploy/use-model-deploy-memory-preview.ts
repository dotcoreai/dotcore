import { useState } from "react";

import { executionTargetName } from "@/domain/execution-target";
import { previewMemory } from "@/features/models/deploy/memory/model-deploy-memory";
import type { ProfileCompatibility } from "@/features/models/profiles/profile-compatibility";
import { localDeploymentConfiguration } from "@/features/models/runtime/local-deployment-configuration";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import { type EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { Model, ModelOptionValues, ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ParameterDefinition, ParameterValues } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { LocalModelDeploymentAdvisory } from "@/gen/dotcore/model/v1/service_pb";
import type { AcceleratorMemoryArchitecture } from "@/gen/dotcore/observation/v1/observation_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import { useKeyedRead } from "@/hooks/use-keyed-read";

import {
  type LocalResourceEstimateValues,
  type ResourceEstimateLoader,
  useResourceEstimate,
} from "./use-resource-estimate";

/**
 * The deploy dialog's memory picture of the launch: the resource estimate of the configuration on
 * the selected target, the memory strip's prediction built on it, and a background deployment
 * preview that names the target's memory architecture, advisories and selected engine release.
 */
export function useModelDeployMemoryPreview({
  applications,
  compatibilityRead,
  customArgumentTokens,
  declaredValues,
  deployments,
  derivedParameterIds,
  engineParameterInputs,
  engineRelease,
  exposedParameters,
  launchValuesPending,
  model,
  name,
  onEstimate,
  onPreviewMemory,
  optionOverrides,
  placement,
  profile,
  profileId,
  selectedExecutionTarget,
  targetsLoading,
  targetsLoadingPresentation,
  values,
}: ModelDeployMemoryPreviewOptions) {
  const placementNodeId = placement?.siteNodeId;
  const placementTargetId = placement?.executionTargetId;

  const estimateConfiguration = profile
    ? localDeploymentConfiguration(profile, engineRelease, {
        customArgumentTokens,
        declaredValues,
        derivedParameterIds,
        inputs: engineParameterInputs,
        optionOverrides,
      })
    : undefined;
  const estimateKey =
    // The selected target decides the estimate of a profile supporting several hardware classes,
    // so the estimate waits for the targets instead of asking twice.
    profile && values && optionOverrides && !targetsLoading && !launchValuesPending
      ? [
          profileId,
          exposedParameters
            .map(
              (parameter) =>
                `${parameter.parameterId}=${engineParameterInputs.get(parameter.parameterId) ?? ""}`,
            )
            .join("|"),
          declaredValues?.values
            .map((value) => `${value.parameterId}=${String(value.value.value)}`)
            .join("|") ?? "",
          JSON.stringify(customArgumentTokens),
          optionOverrides
            .map(
              (override) =>
                `${override.optionId}=${override.values.map((value) => String(value.value.value)).join(",")}`,
            )
            .join("|"),
          placementNodeId ?? "",
          placementTargetId ?? "",
        ].join("\0")
      : undefined;
  // Memory occupancy moves on its own, such as when another model starts, so every compatibility
  // read also refreshes the placement preview.
  const [compatibilityReads, setCompatibilityReads] = useState({
    count: 0,
    read: compatibilityRead,
  });
  if (compatibilityReads.read !== compatibilityRead)
    setCompatibilityReads({
      count: compatibilityReads.count + 1,
      read: compatibilityRead,
    });
  const estimate = useResourceEstimate(
    onEstimate,
    estimateKey && estimateConfiguration
      ? {
          key: estimateKey,
          revision: compatibilityReads.count,
          values: {
            configuration: estimateConfiguration,
            placement: placement ?? undefined,
            profileId,
          },
        }
      : undefined,
  );
  const predictedMemory = deployments
    ? previewMemory({
        applications,
        deployment: { name: name.trim() || model.displayName, profileId },
        deployments,
        // Without a selected profile, such as when none runs on the target, nothing is previewed.
        // The estimate waits for the launch values, so the preview is pending until they land.
        estimate: profile
          ? { ...estimate, pending: estimate.pending || launchValuesPending }
          : { loadingVisible: false, pending: false },
        target: selectedExecutionTarget && {
          executionTargetId: selectedExecutionTarget.executionTargetId,
          name: executionTargetName(selectedExecutionTarget),
          nodeName:
            selectedExecutionTarget.siteNodeDisplayName || selectedExecutionTarget.siteNodeId,
          siteNodeId: selectedExecutionTarget.siteNodeId,
        },
        targetsLoading: targetsLoadingPresentation,
      })
    : undefined;
  // One preview runs per distinct configuration and target.
  const previewKey = estimateKey && placement ? estimateKey : undefined;
  const previewRead = useKeyedRead<MemoryPreviewResult>(
    previewKey && onPreviewMemory && estimateConfiguration && placement
      ? {
          failureLabel: "Could not preview the deployment",
          key: previewKey,
          load: () =>
            onPreviewMemory({ configuration: estimateConfiguration, placement, profileId }).then(
              (preview) => ({ placement, preview, profileId }),
              // The Command step's command preview reports rejections; this background read only
              // drops its facts.
              () => ({ placement, profileId }),
            ),
        }
      : undefined,
  );
  const memoryPreviewResult = previewRead.value;
  // The latest result for the selected target stays visible while a newer one resolves, so the
  // wording does not flicker as parameters are typed; the architecture depends only on the target.
  const memoryPreview =
    placement &&
    memoryPreviewResult?.placement?.executionTargetId === placement.executionTargetId &&
    memoryPreviewResult?.placement?.siteNodeId === placement.siteNodeId
      ? memoryPreviewResult.preview
      : undefined;
  const hostKvOffloadSharesMemory = Boolean(
    memoryPreview?.advisories.includes(LocalModelDeploymentAdvisory.HOST_KV_OFFLOAD_SHARES_MEMORY),
  );
  const poolBelowDeviceMinimum = Boolean(
    memoryPreview?.advisories.includes(
      LocalModelDeploymentAdvisory.UNIFIED_MEMORY_POOL_BELOW_DEVICE_MINIMUM,
    ),
  );
  return {
    estimate,
    hostKvOffloadSharesMemory,
    memoryPreview,
    poolBelowDeviceMinimum,
    predictedMemory,
  };
}

export interface ModelDeployMemoryPreviewOptions {
  applications?: readonly ApplicationDeployment[];
  /** The dialog's latest compatibility read; each new one refreshes the estimate. */
  compatibilityRead?: ProfileCompatibility;
  customArgumentTokens: string[];
  declaredValues?: ParameterValues;
  deployments?: readonly ModelDeployment[];
  /** Parameters catalog sizing derives for the profile, which an empty input leaves to it. */
  derivedParameterIds: ReadonlySet<string>;
  engineParameterInputs: ReadonlyMap<string, string>;
  engineRelease?: EngineRelease;
  exposedParameters: readonly ParameterDefinition[];
  launchValuesPending: boolean;
  model: Model;
  name: string;
  onEstimate?: ResourceEstimateLoader;
  onPreviewMemory?: (values: LocalMemoryPreviewValues) => Promise<LocalDeploymentMemoryPreview>;
  optionOverrides?: ModelOptionValues[];
  placement: ExactPlacement | null;
  profile?: ModelProfile;
  profileId: string;
  selectedExecutionTarget?: ConfiguredExecutionTarget;
  targetsLoading: boolean;
  targetsLoadingPresentation: boolean;
  values?: ParameterValues;
}

export interface LocalMemoryPreviewValues extends LocalResourceEstimateValues {
  placement: ExactPlacement;
}

/**
 * The facts of a background deployment preview: whether the selected execution target shares host
 * memory with its accelerators, the non-blocking advisories for the previewed configuration, and
 * the sizes a memory advisory compares.
 */
export interface LocalDeploymentMemoryPreview {
  acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture;
  advisories: readonly LocalModelDeploymentAdvisory[];
  /** The selected target's memory-pool total, absent when unknown. */
  memoryPoolTotalBytes?: bigint;
  /** The profile's largest per-device accelerator memory minimum, absent when it declares none. */
  minimumMemoryBytesPerDevice?: bigint;
}

interface MemoryPreviewResult {
  preview?: LocalDeploymentMemoryPreview;
  placement: ExactPlacement;
  profileId: string;
}
