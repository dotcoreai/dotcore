import type { ConfiguredExecutionTargetLoader } from "@/features/deployments/use-configured-execution-targets";
import type { RouteNameConflict } from "@/features/models/deployment-exposure";
import type { MemoryBasis } from "@/features/models/profiles/profile-compatibility";
import type { CatalogChips } from "@/features/models/profiles/profile-table-seed";
import type { SizingScenario } from "@/features/models/profiles/sizing-scenario";
import type {
  ProfileCompatibilityLoader,
  ProfileEstimateState,
} from "@/features/models/profiles/use-profile-compatibility";
import type { LocalDeploymentConfigurationValues } from "@/features/models/runtime/local-deployment-configuration";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import type { EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { HardwareClass } from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import type {
  ModelDeployment,
  ResolvedLocalModelDeploymentSnapshot,
} from "@/gen/dotcore/model/v1/deployment_pb";
import type { ModelDeploymentExposure } from "@/gen/dotcore/model/v1/service_pb";

import type { LocalDeploymentValues } from "./model-deploy-launch";
import type {
  LocalDeploymentMemoryPreview,
  LocalMemoryPreviewValues,
} from "./use-model-deploy-memory-preview";
import type { ResourceEstimateLoader } from "./use-resource-estimate";

/** What the deploy form needs from its opener: the model, the site's facts and the API calls. */
export interface ModelDeployFormOptions {
  engineReleases: readonly EngineRelease[];
  hardwareClasses: readonly HardwareClass[];
  existingRouteNames?: readonly string[];
  /**
   * The catalog's chips the profile table starts from, taken when the dialog opened; the catalog's
   * default chips without them.
   */
  catalogChips?: CatalogChips;
  /** The site's application deployments, which name their memory in the memory strip. */
  applications?: readonly ApplicationDeployment[];
  /**
   * The site's model deployments, which name what occupies the selected target's memory in the
   * memory strip. Without them, the strip doesn't show.
   */
  deployments?: readonly ModelDeployment[];
  /**
   * The runtime configuration the launch starts from, such as one the dialog submitted before: its
   * engine parameters, options and custom arguments, until a profile change resets the profile's
   * own. The context comes from `scenario`, and the weights from the profile. The profile's
   * defaults without it.
   */
  initialConfiguration?: LocalDeploymentConfigurationValues;
  /** The exposure the launch starts with, such as one the dialog submitted before; direct without it. */
  initialExposure?: ModelDeploymentExposure;
  /** The deployment name the launch starts with, such as one the operator already chose. */
  initialName?: string;
  /**
   * The target the launch starts on, such as a proposal's, kept while it can run the selected
   * profile; the dialog picks one without it.
   */
  initialPlacement?: ExactPlacement;
  initialProfileId?: string;
  listConfiguredExecutionTargets?: ConfiguredExecutionTargetLoader;
  /**
   * Reads which of the model's profiles can run on each target. Without it, or while it loads or
   * fails, every profile is listed and selectable.
   */
  listProfileCompatibility?: ProfileCompatibilityLoader;
  /** The memory basis profiles are judged on at first, such as the catalog's; free now without it. */
  memoryBasis?: MemoryBasis;
  model: Model;
  /** The route name the backend rejected as taken on the last deploy attempt. */
  routeConflict?: RouteNameConflict;
  onEstimate?: ResourceEstimateLoader;
  onPreview: (values: LocalDeploymentValues) => Promise<ResolvedLocalModelDeploymentSnapshot>;
  onPreviewMemory?: (values: LocalMemoryPreviewValues) => Promise<LocalDeploymentMemoryPreview>;
  onSubmit: (values: LocalDeploymentValues) => void;
  open: boolean;
  pending: boolean;
  /**
   * The opener's profile memory estimates under `scenario`, for the profile table's Memory column.
   * Without them, profiles show their declared minimums; the dialog's own read replaces them once
   * it lands.
   */
  profileEstimates?: ProfileEstimateState;
  /**
   * The sizing scenario the launch's context starts from, such as the catalog's. The profile table
   * reads it for the dialog's life, and the launch starts from its engine parameter values on a
   * target where the profile fits.
   */
  scenario?: SizingScenario;
}
