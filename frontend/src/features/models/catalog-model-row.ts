import { type ValueRange, valueRange } from "@/components/data-table/data-table-model";
import type { EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { HardwareClass } from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import {
  type InferenceRequestKind,
  ModelInputModality,
} from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import type {
  Model,
  ModelEngineConfiguration,
  ModelOption,
  ModelOptionValues,
  ModelProfile,
} from "@/gen/dotcore/catalog/v1/model_pb";
import { EngineKind } from "@/gen/dotcore/engine/v1/engine_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { SizingScenarioUnsupportedReason } from "@/gen/dotcore/model/v1/service_pb";
import { mergeRequestKinds } from "@/lib/request-kinds";
import type { StatusProjection } from "@/lib/status-projection";

import { maximumContext as configurationMaximumContext } from "./engine-parameter-values";
import { localModelSnapshot, projectCatalogModelStatus } from "./model-projection";
import type { MemoryBasis, MemoryFit } from "./profile-compatibility";
import { sizingUnsupportedText } from "./sizing-scenario";

/** Where a profile's accelerator memory value comes from. */
export type AcceleratorMemoryKind = "estimate" | "minimum" | "system-ram";

/** A model catalog entry flattened for the Catalog table, with one row per profile. */
export interface ModelRow {
  colorKey: string;
  displayName: string;
  /** The model's upstream licence identifier. */
  licenseId?: string;
  modelId: string;
  profileCount: number;
  profiles: ProfileRow[];
  /** The model's overall intelligence score, 0–100. */
  referenceScore?: number;
  /** The request kinds any of the model's configurations serves. */
  requestKinds: InferenceRequestKind[];
  runningCount: number;
  runningStatus: StatusProjection | undefined;
  /** Display names of the speculative decoding methods the model offers, in declaration order. */
  speculativeMethods: string[];
  supportsVision: boolean;
}

/** One model profile with the values the table, filters, and inspector read. A missing value is absent. */
export interface ProfileRow {
  acceleratorRequirements: ProfileAcceleratorRequirement[];
  configurationId: string;
  description?: string;
  displayName: string;
  /** Engine kind keys such as "vllm" (see `engineKindLabel`). */
  engineKinds: string[];
  /** The profile's advisory memory fit at the catalog's location, when a target there runs it. */
  fit?: ProfileFit;
  /**
   * The catalog estimate of the profile's untouched configuration on each supported hardware
   * class, in configuration order; empty when estimates are unavailable.
   */
  hardwareClassEstimates: ProfileHardwareClassEstimate[];
  imageInput: boolean;
  /** The largest context one request may use on the profile's configuration, in tokens. */
  maximumContext?: number;
  /**
   * The accelerator memory of the profile at the catalog's location: the estimate of the hardware
   * classes its targets there select, or their declared minimum when they have none, else a range
   * across every supported class when no target there runs it. Absent when the profile can't serve
   * the scenario.
   */
  memory?: ProfileMemory;
  /** The catalog model the profile belongs to, which compatibility entries are keyed by. */
  modelId: string;
  platforms: ProfilePlatform[];
  profileId: string;
  /** The default binding's weight quantization label. */
  quantization?: string;
  /** The model's reference score, carried by each profile because the table sorts on profiles. */
  referenceScore?: number;
  /** Why the profile can't serve the sizing scenario on any hardware class. */
  scenarioUnsupported?: string;
  /** The supported hardware classes the profile can't serve the sizing scenario on. */
  scenarioUnsupportedClasses: { displayName: string; hardwareClassId: string }[];
  /** The profile's speculative options with the count range and the profile's effective default. */
  speculativeOptions: SpeculativeOption[];
  /**
   * The total context the profile is shown at: the sizing scenario's, or without one the largest
   * that fits the location's machines that run it. Absent when unknown or nothing fits.
   */
  totalContext?: number;
}

/**
 * A profile's advisory memory fit at a location on a memory basis, with why when it can't serve the
 * scenario.
 */
export interface ProfileFit {
  basis: MemoryBasis;
  fit: MemoryFit;
  /** The location within a sentence, such as "this site". */
  location: string;
  reason?: string;
}

/**
 * One speculative decoding option as a configuration declares it: the count range in tokens, where
 * 0 is off, and the default count the configuration or profile resolves to.
 */
export interface SpeculativeOption {
  defaultCount: number;
  /** The option's display name, which names its draft method, such as "MTP". */
  displayName: string;
  maximum: number;
  minimum: number;
  optionId: string;
}

/** A profile's accelerator memory in bytes, one value or a range, with where it comes from. */
export interface ProfileMemory extends ValueRange {
  kind: AcceleratorMemoryKind;
}

/** One supported hardware class's estimate of a profile, with the class's display name. */
export interface ProfileHardwareClassEstimate extends HardwareClassEstimate {
  /** Display name of the hardware class, or its id when the class is unknown. */
  displayName: string;
}

export interface ProfilePlatform {
  architecture: string;
  operatingSystem: string;
}

export interface ProfileAcceleratorRequirement {
  /** Display name of the supported hardware class, or its id when the class is unknown. */
  displayName: string;
  hardwareClassId: string;
  minimumMemoryBytesPerDevice: number;
  /** Accelerator vendors the class selects, any one of them, such as "nvidia". */
  vendors: string[];
}

/** The memory estimates of one configuration on one supported hardware class, in bytes. */
export interface HardwareClassEstimate {
  acceleratorMemoryBytes: bigint;
  hardwareClassId: string;
  /** Pinned host memory beside the accelerator memory, such as an offloaded prompt cache. */
  hostMemoryBytes?: bigint;
}

/** Per-class memory estimates of each profile, keyed by `profileEstimateKey`. */
export type ProfileEstimates = ReadonlyMap<string, readonly HardwareClassEstimate[]>;

export interface ModelRowInput extends ProfileRowInput {
  deployments: readonly ModelDeployment[];
  models: readonly Model[];
}

export interface ProfileRowInput {
  engineReleases: readonly EngineRelease[];
  hardwareClasses: readonly HardwareClass[];
  estimates: ProfileEstimates;
  /** The sizing read at the catalog's location; without it, memory spans every supported class. */
  sizing?: ProfileSizingView;
}

/** What a sizing read says about each profile at one location: the site or one machine. */
export interface ProfileSizingView {
  /** The memory basis fits are judged on. */
  basis: MemoryBasis;
  /** The hardware classes the location's targets select for a profile, empty when none runs it. */
  classIds: (modelId: string, profileId: string) => readonly string[];
  /** A profile's memory fit at the location, absent when no target there runs it. */
  fit: (
    modelId: string,
    profileId: string,
  ) => { fit: MemoryFit; unsupportedReason?: SizingScenarioUnsupportedReason } | undefined;
  /** The location within a sentence, such as "this site". */
  location: string;
  /** The total context a profile is shown at, absent when unknown. */
  totalContext: (modelId: string, profileId: string) => number | undefined;
  /** The supported hardware classes a profile can't serve the scenario on. */
  unsupportedClassIds: (modelId: string, profileId: string) => readonly string[];
  /** Why a profile can't serve the scenario on any hardware class. */
  unsupportedReason: (
    modelId: string,
    profileId: string,
  ) => SizingScenarioUnsupportedReason | undefined;
}

/** The parameter id the catalog fixes for the count of every speculative decoding option. */
export const speculativeTokenCountId = "token-count";

export function projectModelRows(input: ModelRowInput): ModelRow[] {
  return input.models.map((model) => projectModelRow(model, input));
}

/** The local model deployments made from the catalog model, in any profile. */
export function catalogModelDeployments(
  modelId: string,
  deployments: readonly ModelDeployment[],
): ModelDeployment[] {
  return deployments.filter(
    (deployment) => localModelSnapshot(deployment.snapshot)?.modelId === modelId,
  );
}

/** The profiles of one model, in catalog order. */
export function projectProfileRows(model: Model, input: ProfileRowInput): ProfileRow[] {
  return model.profiles.map((profile) => projectProfileRow(model, profile, input));
}

/** The speculative decoding options a model declares. */
export function modelSpeculativeOptions(model: Model): ModelOption[] {
  return model.options.filter((option) => option.kind.case === "speculativeDecoding");
}

/**
 * The speculative options a configuration declares, in model declaration order, with the count
 * range and the configuration's default. A profile's option defaults replace the default.
 */
export function configurationSpeculativeOptions(
  model: Model,
  configuration: ModelEngineConfiguration | undefined,
  profile?: ModelProfile,
): SpeculativeOption[] {
  return modelSpeculativeOptions(model).flatMap((option) => {
    const declared = configuration?.options.find(
      (candidate) => candidate.optionId === option.optionId,
    );
    const range = declared?.parameters.find(
      (parameter) => parameter.parameterId === speculativeTokenCountId,
    )?.parameterType;
    if (!declared || range?.case !== "unsignedInteger") return [];

    const profileDefaults = profile?.optionDefaults.find(
      (candidate) => candidate.optionId === option.optionId,
    );
    return [
      {
        defaultCount: Number(
          tokenCount(profileDefaults) ?? tokenCount(declared.defaultValues) ?? 0n,
        ),
        displayName: option.displayName,
        maximum: Number(range.value.maximum),
        minimum: Number(range.value.minimum),
        optionId: option.optionId,
      },
    ];
  });
}

/**
 * The default speculative state of a profile: the first option defaulting on as "MTP, 3 tokens",
 * "Off" when every option defaults to 0, and undefined without a speculative option.
 */
export function formatSpeculativeDefault(
  options: readonly SpeculativeOption[],
): string | undefined {
  if (!options.length) return undefined;

  const active = options.find((option) => option.defaultCount > 0);
  return active ? `${active.displayName}, ${formatTokens(active.defaultCount)}` : "Off";
}

/** The method a profile defaults to, such as `["MTP"]` or `["Off"]`; empty without a speculative option. */
export function speculativeDefaultMethod(options: readonly SpeculativeOption[]): string[] {
  if (!options.length) return [];

  return [options.find((option) => option.defaultCount > 0)?.displayName ?? "Off"];
}

/** The count range of a speculative option, such as "0–8 tokens". */
export function formatSpeculativeRange(option: SpeculativeOption): string {
  return `${option.minimum}–${formatTokens(option.maximum)}`;
}

export function profileEstimateKey(modelId: string, profileId: string): string {
  return `${modelId}\0${profileId}`;
}

/** The lowercase key of an engine kind, such as "llama-cpp". */
export function engineKindKey(kind: EngineKind): string | undefined {
  if (kind === EngineKind.UNSPECIFIED) return undefined;
  return EngineKind[kind]?.toLowerCase().replaceAll("_", "-");
}

/** The product name of an engine kind key; unknown keys read as themselves. */
export function engineKindLabel(key: string): string {
  return engineKindLabels[key] ?? key;
}

const engineKindLabels: Record<string, string> = {
  "llama-cpp": "llama.cpp",
  ninfer: "ninfer",
  vllm: "vLLM",
};

function projectModelRow(model: Model, input: ModelRowInput): ModelRow {
  const runningCount = catalogModelDeployments(model.modelId, input.deployments).length;
  const profiles = projectProfileRows(model, input);

  return {
    colorKey: model.modelId,
    displayName: model.displayName,
    licenseId: model.license?.id,
    modelId: model.modelId,
    profileCount: profiles.length,
    profiles,
    referenceScore: model.intelligence?.overall,
    requestKinds: mergeRequestKinds(model.configurations.map((c) => c.requestKinds)),
    runningCount,
    runningStatus: projectCatalogModelStatus(runningCount),
    speculativeMethods: modelSpeculativeOptions(model).map((option) => option.displayName),
    supportsVision:
      model.options.some((option) => option.kind.case === "vision") ||
      model.configurations.some((configuration) => supportsImageInput(model, configuration)),
  };
}

function projectProfileRow(
  model: Model,
  profile: ModelProfile,
  input: ProfileRowInput,
): ProfileRow {
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile.configurationId,
  );
  const releases = (configuration?.engineArtifacts ?? []).flatMap((reference) => {
    const release = input.engineReleases.find(
      (candidate) => candidate.engineReleaseId === reference.engineReleaseId,
    );
    return release ? [{ artifactId: reference.engineArtifactId, release }] : [];
  });
  const binding = configuration?.artifactBindings.find(
    (candidate) => candidate.bindingId === profile.defaultModelArtifactBindingId,
  );
  const artifact = model.artifacts.find(
    (candidate) => candidate.artifactId === binding?.modelArtifactId,
  );
  const acceleratorRequirements = (configuration?.hardwareSupport ?? []).flatMap((support) => {
    const hardwareClass = input.hardwareClasses.find(
      (candidate) => candidate.hardwareClassId === support.hardwareClassId,
    );
    const vendors = hardwareClass?.selector?.accelerators ?? [];
    return hardwareClass && vendors.length
      ? [
          {
            displayName: hardwareClass.displayName || support.hardwareClassId,
            hardwareClassId: support.hardwareClassId,
            minimumMemoryBytesPerDevice: Number(
              support.hardwareRequirements?.minimumMemoryBytesPerDevice ?? 0n,
            ),
            vendors: [...vendors],
          },
        ]
      : [];
  });

  const classDisplayName = (hardwareClassId: string) =>
    input.hardwareClasses.find((candidate) => candidate.hardwareClassId === hardwareClassId)
      ?.displayName || hardwareClassId;
  const hardwareClassEstimates = (
    input.estimates.get(profileEstimateKey(model.modelId, profile.profileId)) ?? []
  ).map((estimate) => ({ ...estimate, displayName: classDisplayName(estimate.hardwareClassId) }));

  const engineKinds = unique([
    ...(configuration ? engineKindKeys(configuration.engineConfiguration?.engineKind) : []),
    ...releases.flatMap(({ release }) => engineKindKeys(release.engineKind)),
  ]);
  const { sizing } = input;
  const maximumContext = configurationMaximumContext(model, configuration);
  const unsupportedText = (reason: SizingScenarioUnsupportedReason) =>
    sizingUnsupportedText(reason, {
      engine: engineKinds.map(engineKindLabel).join(", "),
      maximumContextTokens: maximumContext,
    });
  const unsupportedReason = sizing?.unsupportedReason(model.modelId, profile.profileId);
  const fit = sizing?.fit(model.modelId, profile.profileId);
  // The classes the location's targets select; without any, every supported class applies.
  const locationClasses = sizing?.classIds(model.modelId, profile.profileId) ?? [];
  const atLocation = <T extends { hardwareClassId: string }>(entries: readonly T[]) =>
    locationClasses.length
      ? entries.filter(({ hardwareClassId }) => locationClasses.includes(hardwareClassId))
      : [...entries];

  return {
    acceleratorRequirements,
    configurationId: profile.configurationId,
    description: profile.description || undefined,
    displayName: profile.displayName || profile.profileId,
    engineKinds,
    fit: fit && {
      basis: sizing!.basis,
      fit: fit.fit,
      location: sizing!.location,
      reason:
        fit.unsupportedReason === undefined ? undefined : unsupportedText(fit.unsupportedReason),
    },
    hardwareClassEstimates,
    imageInput: configuration ? supportsImageInput(model, configuration) : false,
    maximumContext,
    memory:
      unsupportedReason === undefined
        ? profileMemory(
            atLocation(hardwareClassEstimates),
            atLocation(acceleratorRequirements),
            acceleratorRequirements.length === 0,
            configuration,
          )
        : undefined,
    modelId: model.modelId,
    platforms: uniquePlatforms(
      releases.flatMap(({ artifactId, release }) =>
        release.artifacts
          .filter((candidate) => candidate.artifactId === artifactId)
          .flatMap((candidate) => candidate.selectors.flatMap((entry) => entry.platform ?? []))
          .map((platform) => ({
            architecture: platform.architecture,
            operatingSystem: platform.operatingSystem,
          })),
      ),
    ),
    profileId: profile.profileId,
    quantization: artifact?.weightQuantization || undefined,
    referenceScore: model.intelligence?.overall,
    scenarioUnsupported:
      unsupportedReason === undefined ? undefined : unsupportedText(unsupportedReason),
    scenarioUnsupportedClasses: (
      sizing?.unsupportedClassIds(model.modelId, profile.profileId) ?? []
    ).map((hardwareClassId) => ({
      displayName: classDisplayName(hardwareClassId),
      hardwareClassId,
    })),
    speculativeOptions: configurationSpeculativeOptions(model, configuration, profile),
    totalContext: sizing?.totalContext(model.modelId, profile.profileId),
  };
}

function formatTokens(count: number): string {
  return `${count} ${count === 1 ? "token" : "tokens"}`;
}

function tokenCount(values: ModelOptionValues | undefined): bigint | undefined {
  const value = values?.values.find(
    (candidate) => candidate.parameterId === speculativeTokenCountId,
  )?.value;
  return value?.case === "unsignedInteger" ? value.value : undefined;
}

// Estimates span the given classes; without any, their largest declared minimum stands in, and a
// profile without accelerators reads its system memory.
function profileMemory(
  estimates: readonly HardwareClassEstimate[],
  requirements: readonly ProfileAcceleratorRequirement[],
  cpuOnly: boolean,
  configuration: ModelEngineConfiguration | undefined,
): ProfileMemory | undefined {
  const systemMemory = Math.max(
    0,
    ...(configuration?.hardwareSupport ?? []).map((support) =>
      Number(support.hardwareRequirements?.minimumSystemMemoryBytes ?? 0n),
    ),
  );
  if (cpuOnly && systemMemory > 0) {
    return { kind: "system-ram", max: systemMemory, min: systemMemory };
  }

  const estimated = valueRange(
    estimates.flatMap(({ acceleratorMemoryBytes }) =>
      acceleratorMemoryBytes > 0n ? [Number(acceleratorMemoryBytes)] : [],
    ),
  );
  if (estimated) return { ...estimated, kind: "estimate" };

  const minimum = Math.max(
    0,
    ...requirements.map((requirement) => requirement.minimumMemoryBytesPerDevice),
  );
  return minimum > 0 ? { kind: "minimum", max: minimum, min: minimum } : undefined;
}

// A configuration takes images when it declares the image modality or implements a vision option.
function supportsImageInput(model: Model, configuration: ModelEngineConfiguration): boolean {
  return (
    configuration.inputCapabilities?.supportedModalities.includes(ModelInputModality.IMAGE) ||
    configuration.options.some(
      (option) =>
        model.options.find((candidate) => candidate.optionId === option.optionId)?.kind.case ===
        "vision",
    )
  );
}

function engineKindKeys(kind: EngineKind | undefined): string[] {
  const key = kind === undefined ? undefined : engineKindKey(kind);
  return key ? [key] : [];
}

function unique<T>(values: readonly T[]): T[] {
  return [...new Set(values)];
}

function uniquePlatforms(platforms: readonly ProfilePlatform[]): ProfilePlatform[] {
  const seen = new Set<string>();
  return platforms.filter((platform) => {
    const key = `${platform.operatingSystem}/${platform.architecture}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}
