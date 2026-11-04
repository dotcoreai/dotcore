import type { ModelEngineConfiguration } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";

export interface ModelEngineDiagnostics {
  gpuLayers: number;
  flashAttention: boolean;
  visionAuxiliaryArtifactId?: string;
}

export function catalogModelEngineDiagnostics(
  configuration: ModelEngineConfiguration | undefined,
): ModelEngineDiagnostics | undefined {
  const engineConfiguration = configuration?.engineConfiguration?.configuration;
  if (engineConfiguration?.case !== "llamaCpp") {
    return undefined;
  }

  const settings = engineConfiguration.value.fixedSettings;
  if (!settings) {
    return undefined;
  }

  return {
    gpuLayers: settings.gpuLayers,
    flashAttention: settings.flashAttention,
    visionAuxiliaryArtifactId: settings.visionProjector?.auxiliaryArtifactId,
  };
}

export function resolvedModelEngineDiagnostics(
  snapshot: ResolvedLocalModelDeploymentSnapshot,
): ModelEngineDiagnostics | undefined {
  const engineState = snapshot.resolvedEngineState?.state;
  if (engineState?.case !== "llamaCpp") {
    return undefined;
  }

  const settings = engineState.value.fixedSettings;
  if (!settings) {
    return undefined;
  }

  return {
    gpuLayers: settings.gpuLayers,
    flashAttention: settings.flashAttention,
    visionAuxiliaryArtifactId: settings.visionProjector?.auxiliaryArtifactId,
  };
}
